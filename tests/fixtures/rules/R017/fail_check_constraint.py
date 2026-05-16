# R017: AddConstraint with a CHECK constraint validates all rows - should fail
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0001_initial'),
    ]

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(
                check=models.Q(price__gte=0),
                name='product_price_non_negative',
            ),
        ),
    ]
