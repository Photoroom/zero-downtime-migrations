# R010: AddField NOT NULL without default - should fail
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0007'),
    ]

    operations = [
        migrations.AddField(
            model_name='order',
            name='required_field',
            field=models.CharField(max_length=100, null=False),
        ),
    ]
