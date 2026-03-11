# R002: AddConstraint with UniqueConstraint - should fail
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0004'),
    ]

    operations = [
        migrations.AddConstraint(
            model_name='order',
            constraint=models.UniqueConstraint(
                fields=['external_id'],
                name='order_external_id_unique',
            ),
        ),
    ]
